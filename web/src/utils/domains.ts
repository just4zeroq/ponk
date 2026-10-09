/**
 * 八域的本地兜底元数据（09 §2 / §2.2）。
 *
 * 优先用 `GET /meta`（docs/13 §4.3）；后端未返回某字段时用本表补位，
 * 保证「中文名 / 消费者 / 传播类别」始终可展示。
 */
import type { ConfigDomainName, PushClass } from '../api/types'

export interface DomainLocal {
  name: ConfigDomainName
  display: string
  consumers: string[]
  push: PushClass
  /** 09 §5.4：发布触发指派重算的三域。 */
  recompute: boolean
  /** 一句话说明（P6 列表 / P0 卡片用）。 */
  summary: string
}

export const DOMAINS: DomainLocal[] = [
  {
    name: 'subscribe_plan',
    display: '订阅计划',
    consumers: ['02 采集（唯一外部消费者）', '控制面指派器（派生输入）'],
    push: 'B',
    recompute: true,
    summary: '标的候选集（白名单）、通道集、分片参数、instrument meta、配额份额',
  },
  {
    name: 'signal_rules',
    display: '信号规则',
    consumers: ['05 信号引擎'],
    push: 'A',
    recompute: false,
    summary: 'RuleGraph DAG、算子参数、作用标的、输出映射',
  },
  {
    name: 'alert_rules',
    display: '告警规则',
    consumers: ['06 监控告警'],
    push: 'A',
    recompute: false,
    summary: 'match / when / window / level / route / throttle',
  },
  {
    name: 'factor_rules',
    display: '因子规则',
    consumers: ['07 因子挖掘'],
    push: 'A',
    recompute: false,
    summary: '输入通道、窗口、计算算子、输出因子',
  },
  {
    name: 'channels',
    display: '渠道配置',
    consumers: ['06 监控告警（路由目的地）'],
    push: 'A',
    recompute: false,
    summary: '飞书 / 钉钉 backend、endpoint 与凭据、重试与限额',
  },
  {
    name: 'process_capacity',
    display: '进程承载上限',
    consumers: ['控制面指派器（无外部消费者）'],
    push: 'C',
    recompute: true,
    summary: '每进程最大分片数 / 标的数 / 通道数、分布策略、按 host 分散策略',
  },
  {
    name: 'serialization',
    display: '序列化格式',
    consumers: ['02 / 04 / 05 / 06 / 07 及各节点内嵌事件中心适配层'],
    push: 'A',
    recompute: false,
    summary: '按主题族的格式取值；ctl.* / ops.* 固定 JSON',
  },
  {
    name: 'instrument_blocklist',
    display: '交易对黑名单',
    consumers: ['02 采集（执行）', '控制面指派器（标的集计算）', '05 / 07 可选过滤'],
    push: 'A',
    recompute: true,
    summary: '标的级排除集（下架 / 暂停 / 风险标的）',
  },
]

const BY_NAME = new Map(DOMAINS.map((d) => [d.name as string, d]))

/** 按 wire 名取本地域定义。 */
export function domainLocal(name: string): DomainLocal | undefined {
  return BY_NAME.get(name)
}

/** 传播类别中文说明（09 §2.2）。 */
export const PUSH_TEXT: Record<PushClass, string> = {
  A: 'A：广播通知 + 定向应答',
  B: 'B：A + 定向推送',
  C: 'C：不发通知（效果以 assignment_version 体现）',
}
