import type { Accessor, JSX } from 'solid-js'
import { createContext, createEffect, createSignal, useContext } from 'solid-js'

export type Locale = 'en' | 'zh-CN'

const EN = {
  'brand.subtitle': 'Workspace',
  'language.label': 'Language',
  'language.english': 'English',
  'language.chinese': 'Chinese',
  'nav.primary': 'Primary navigation',
  'nav.reviews': 'Reviews',
  'nav.contacts': 'Contacts',
  'nav.policies': 'Access rules',
  'app.connecting': 'Connecting',
  'common.loading': 'Loading',
  'common.couldNotLoad': 'Could not load data',
  'common.retry': 'Retry',
  'common.pagination': 'Pagination',
  'common.pageSummary': 'Page {page} of {pages} · {total} total',
  'common.previousPage': 'Previous page',
  'common.nextPage': 'Next page',
  'common.closeDialog': 'Close dialog',
  'common.close': 'Close',
  'common.dismissMessage': 'Dismiss message',
  'common.refresh': 'Refresh',
  'common.actions': 'Actions',
  'common.none': 'None',
  'common.cancel': 'Cancel',
  'common.delete': 'Delete',
  'common.deleting': 'Deleting',
  'common.saving': 'Saving',
  'common.edit': 'Edit',
  'common.notSet': 'Not set',
  'common.invalidDate': 'Invalid date',
  'common.unexpectedError': 'An unexpected error occurred',
  'status.allow': 'Allow',
  'status.review': 'Review',
  'status.deny': 'Deny',
  'status.pending': 'Pending',
  'status.active': 'Active',
  'status.transfered': 'Transferred',
  'status.expired': 'Expired',
  'status.blocked': 'Blocked',
  'reviews.eyebrow': 'Decision queue',
  'reviews.title': 'Reviews',
  'reviews.section': 'Pending reviews',
  'reviews.updating': 'Updating',
  'reviews.pendingCount': '{count} pending',
  'reviews.loading': 'Loading reviews',
  'reviews.clear': 'Queue is clear',
  'reviews.noneWaiting': 'No reviews are waiting.',
  'reviews.request': 'Request',
  'reviews.visitor': 'Visitor',
  'reviews.target': 'Target',
  'reviews.reason': 'Reason',
  'reviews.expiry': 'Expiry',
  'reviews.allow': 'Allow',
  'reviews.deny': 'Deny',
  'reviews.allowRequest': 'Allow request',
  'reviews.denyRequest': 'Deny request',
  'reviews.submitting': 'Submitting',
  'reviews.decisionTitle': '{action} request #{id}',
  'reviews.decisionExpires': 'Decision expires',
  'reviews.allowed': 'Request allowed',
  'reviews.denied': 'Request denied',
  'reviews.alreadyResolved': 'The request was already resolved',
  'contacts.eyebrow': 'Identity directory',
  'contacts.title': 'Contacts',
  'contacts.count': '{count} contacts',
  'contacts.selected': '{count} selected',
  'contacts.sortBy': 'Sort contacts by',
  'contacts.updated': 'Updated',
  'contacts.created': 'Created',
  'contacts.name': 'Name',
  'contacts.alias': 'Alias',
  'contacts.class': 'Class',
  'contacts.sortDirection': 'Sort direction',
  'contacts.descending': 'Descending',
  'contacts.ascending': 'Ascending',
  'contacts.section': 'Contacts',
  'contacts.loading': 'Loading contacts',
  'contacts.empty': 'No contacts',
  'contacts.emptyDetail': 'Incoming contact applications will appear here.',
  'contacts.selectAll': 'Select all contacts on this page',
  'contacts.select': 'Select {name}',
  'contacts.contact': 'Contact',
  'contacts.status': 'Status',
  'contacts.expires': 'Expires',
  'contacts.open': 'Open',
  'contacts.openContact': 'Open {name}',
  'contacts.openDetails': 'Open details',
  'contacts.unclassified': 'Unclassified',
  'contacts.noDescription': 'No description',
  'contacts.details': 'Contact details',
  'contacts.closeDetails': 'Close contact details',
  'contacts.loadingOne': 'Loading contact',
  'contacts.subjectId': 'Subject ID',
  'contacts.localAlias': 'Local alias',
  'contacts.saveAlias': 'Save alias',
  'contacts.aliasUpdated': 'Alias updated',
  'contacts.requestedAccess': 'Requested access',
  'contacts.grantedAccess': 'Granted access',
  'contacts.declaredAccess': 'Declared access',
  'contacts.approveTitle': 'Approve contact',
  'contacts.approved': 'Contact approved',
  'contacts.approve': 'Approve',
  'contacts.block': 'Block',
  'contacts.blocked': 'Contact blocked',
  'contacts.restore': 'Restore',
  'contacts.restored': 'Contact restored',
  'contacts.deleteTitle': 'Delete contacts ({count})',
  'contacts.deleteDetail': 'This also removes exact-subject access rules for the selected contacts.',
  'contacts.deletedOne': '1 contact deleted',
  'contacts.deletedMany': '{count} contacts deleted',
  'policies.eyebrow': 'Authorization policy',
  'policies.title': 'Access rules',
  'policies.add': 'Add rule',
  'policies.organization': 'Rule organization',
  'policies.byApi': 'By API',
  'policies.byGrantee': 'By grantee',
  'policies.explicitCount': '{count} explicit rules',
  'policies.loading': 'Loading access rules',
  'policies.apis': 'APIs',
  'policies.grantees': 'Grantees',
  'policies.noApis': 'No APIs',
  'policies.noGrantees': 'No grantees',
  'policies.selectedRules': 'Selected access rules',
  'policies.api': 'API',
  'policies.grantee': 'Grantee',
  'policies.noSelection': 'No selection',
  'policies.noExplicit': 'No explicit rules',
  'policies.method': 'Method',
  'policies.anyMethod': 'Any method (*)',
  'policies.effect': 'Effect',
  'policies.editorTitle': 'Access rule',
  'policies.apiPath': 'API path',
  'policies.save': 'Save rule',
  'policies.saved': 'Access rule saved',
  'policies.deleteTitle': 'Delete access rule',
  'policies.deleteRule': 'Delete rule',
  'policies.deleted': 'Access rule deleted',
  'policies.lockoutWarning': 'Management rules that remove the last allowed administrator will be rejected.',
} as const

export type MessageKey = keyof typeof EN
type Params = Record<string, string | number>

const ZH_CN: Record<MessageKey, string> = {
  'brand.subtitle': '工作台',
  'language.label': '语言',
  'language.english': '英文',
  'language.chinese': '中文',
  'nav.primary': '主导航',
  'nav.reviews': '审批',
  'nav.contacts': '联系人',
  'nav.policies': '访问规则',
  'app.connecting': '正在连接',
  'common.loading': '加载中',
  'common.couldNotLoad': '无法加载数据',
  'common.retry': '重试',
  'common.pagination': '分页',
  'common.pageSummary': '第 {page} / {pages} 页 · 共 {total} 项',
  'common.previousPage': '上一页',
  'common.nextPage': '下一页',
  'common.closeDialog': '关闭对话框',
  'common.close': '关闭',
  'common.dismissMessage': '关闭消息',
  'common.refresh': '刷新',
  'common.actions': '操作',
  'common.none': '无',
  'common.cancel': '取消',
  'common.delete': '删除',
  'common.deleting': '正在删除',
  'common.saving': '正在保存',
  'common.edit': '编辑',
  'common.notSet': '未设置',
  'common.invalidDate': '无效日期',
  'common.unexpectedError': '发生未知错误',
  'status.allow': '允许',
  'status.review': '审批',
  'status.deny': '拒绝',
  'status.pending': '待处理',
  'status.active': '正常',
  'status.transfered': '已转移',
  'status.expired': '已过期',
  'status.blocked': '已拉黑',
  'reviews.eyebrow': '决策队列',
  'reviews.title': '审批',
  'reviews.section': '待处理审批',
  'reviews.updating': '正在更新',
  'reviews.pendingCount': '{count} 项待处理',
  'reviews.loading': '正在加载审批',
  'reviews.clear': '队列已清空',
  'reviews.noneWaiting': '没有待处理的审批。',
  'reviews.request': '请求',
  'reviews.visitor': '访问者',
  'reviews.target': '目标',
  'reviews.reason': '原因',
  'reviews.expiry': '有效期',
  'reviews.allow': '允许',
  'reviews.deny': '拒绝',
  'reviews.allowRequest': '允许请求',
  'reviews.denyRequest': '拒绝请求',
  'reviews.submitting': '正在提交',
  'reviews.decisionTitle': '{action}请求 #{id}',
  'reviews.decisionExpires': '决定有效期',
  'reviews.allowed': '已允许请求',
  'reviews.denied': '已拒绝请求',
  'reviews.alreadyResolved': '该请求已处理',
  'contacts.eyebrow': '身份目录',
  'contacts.title': '联系人',
  'contacts.count': '{count} 位联系人',
  'contacts.selected': '已选择 {count} 项',
  'contacts.sortBy': '联系人排序字段',
  'contacts.updated': '更新时间',
  'contacts.created': '创建时间',
  'contacts.name': '名称',
  'contacts.alias': '别名',
  'contacts.class': '类型',
  'contacts.sortDirection': '排序方向',
  'contacts.descending': '降序',
  'contacts.ascending': '升序',
  'contacts.section': '联系人',
  'contacts.loading': '正在加载联系人',
  'contacts.empty': '暂无联系人',
  'contacts.emptyDetail': '收到联系人申请后会显示在这里。',
  'contacts.selectAll': '选择本页全部联系人',
  'contacts.select': '选择 {name}',
  'contacts.contact': '联系人',
  'contacts.status': '状态',
  'contacts.expires': '过期时间',
  'contacts.open': '打开',
  'contacts.openContact': '打开 {name}',
  'contacts.openDetails': '打开详情',
  'contacts.unclassified': '未分类',
  'contacts.noDescription': '暂无描述',
  'contacts.details': '联系人详情',
  'contacts.closeDetails': '关闭联系人详情',
  'contacts.loadingOne': '正在加载联系人',
  'contacts.subjectId': '主体标识',
  'contacts.localAlias': '本地别名',
  'contacts.saveAlias': '保存别名',
  'contacts.aliasUpdated': '别名已更新',
  'contacts.requestedAccess': '申请的权限',
  'contacts.grantedAccess': '已授予权限',
  'contacts.declaredAccess': '对方声明权限',
  'contacts.approveTitle': '批准联系人',
  'contacts.approved': '联系人已批准',
  'contacts.approve': '批准',
  'contacts.block': '拉黑',
  'contacts.blocked': '联系人已拉黑',
  'contacts.restore': '恢复',
  'contacts.restored': '联系人已恢复',
  'contacts.deleteTitle': '删除联系人（{count}）',
  'contacts.deleteDetail': '同时会删除所选联系人的精确主体访问规则。',
  'contacts.deletedOne': '已删除 1 位联系人',
  'contacts.deletedMany': '已删除 {count} 位联系人',
  'policies.eyebrow': '授权策略',
  'policies.title': '访问规则',
  'policies.add': '添加规则',
  'policies.organization': '规则组织方式',
  'policies.byApi': '按 API',
  'policies.byGrantee': '按主体',
  'policies.explicitCount': '{count} 条显式规则',
  'policies.loading': '正在加载访问规则',
  'policies.apis': 'API',
  'policies.grantees': '主体',
  'policies.noApis': '暂无 API',
  'policies.noGrantees': '暂无主体',
  'policies.selectedRules': '选中的访问规则',
  'policies.api': 'API',
  'policies.grantee': '主体',
  'policies.noSelection': '未选择',
  'policies.noExplicit': '暂无显式规则',
  'policies.method': '方法',
  'policies.anyMethod': '任意方法（*）',
  'policies.effect': '效果',
  'policies.editorTitle': '访问规则',
  'policies.apiPath': 'API 路径',
  'policies.save': '保存规则',
  'policies.saved': '访问规则已保存',
  'policies.deleteTitle': '删除访问规则',
  'policies.deleteRule': '删除规则',
  'policies.deleted': '访问规则已删除',
  'policies.lockoutWarning': '移除最后一位允许访问的管理员时，管理规则会被拒绝。',
}

export type Translator = (key: MessageKey, params?: Params) => string

interface I18nValue {
  locale: Accessor<Locale>
  setLocale: (locale: Locale) => void
  t: Translator
  date: (value: number | string | null) => string
}

const STORAGE_KEY = 'pishoo.workspace.locale'
const I18nContext = createContext<I18nValue>()

function initialLocale(): Locale {
  const saved = window.localStorage.getItem(STORAGE_KEY)
  if (saved === 'en' || saved === 'zh-CN') return saved
  return navigator.languages.some((language) => language.toLowerCase().startsWith('zh'))
    ? 'zh-CN'
    : 'en'
}

function interpolate(message: string, params?: Params): string {
  if (!params) return message
  return message.replace(/\{(\w+)\}/g, (placeholder, name: string) =>
    params[name] === undefined ? placeholder : String(params[name]),
  )
}

export function I18nProvider(props: { children: JSX.Element }) {
  const [locale, setLocale] = createSignal<Locale>(initialLocale())
  const messages = () => (locale() === 'zh-CN' ? ZH_CN : EN)
  const t: Translator = (key, params) => interpolate(messages()[key], params)
  const date = (value: number | string | null): string => {
    if (value === null) return t('common.notSet')
    const parsed = typeof value === 'number' ? new Date(value * 1000) : new Date(value)
    if (Number.isNaN(parsed.getTime())) return t('common.invalidDate')
    return new Intl.DateTimeFormat(locale() === 'zh-CN' ? 'zh-CN' : 'en-US', {
      dateStyle: 'medium',
      timeStyle: 'short',
    }).format(parsed)
  }

  createEffect(() => {
    document.documentElement.lang = locale()
    window.localStorage.setItem(STORAGE_KEY, locale())
  })

  return (
    <I18nContext.Provider value={{ locale, setLocale, t, date }}>
      {props.children}
    </I18nContext.Provider>
  )
}

export function useI18n(): I18nValue {
  const value = useContext(I18nContext)
  if (!value) throw new Error('I18nProvider is missing')
  return value
}

const STATUS_KEYS: Record<string, MessageKey> = {
  allow: 'status.allow',
  review: 'status.review',
  deny: 'status.deny',
  pending: 'status.pending',
  active: 'status.active',
  transfered: 'status.transfered',
  expired: 'status.expired',
  blocked: 'status.blocked',
}

export function statusLabel(t: Translator, value: string): string {
  const key = STATUS_KEYS[value]
  return key ? t(key) : value
}
