import type { GatewayClient } from './gateway.ts'
import type { StateClient } from './state.ts'
import type { Envelope, ManifestInput, ValidateResponse } from './types.ts'

/**
 * 一次调用的上下文。
 *
 * 它是 Go 侧 `context.Context` 的对应物，只保留插件真正需要的两件事：
 * 还剩多少预算、以及中台有没有取消这次调用。
 */
export interface CallContext {
  /**
   * 距信封 deadline 还剩多少毫秒。信封没带 deadline 时是 `Infinity`。
   *
   * 逐跳递减的绝对 deadline 意味着：紧张时应当**提前放弃**，让失败快速冒泡，
   * 而不是把时间耗光后靠上层超时兜底——那样调用方连「为什么慢」都看不出来。
   */
  readonly budgetMs: number

  /** 中台取消这次调用时触发（客户端断连、上游超时）。 */
  readonly signal: AbortSignal
}

/**
 * 插件必须实现的接口。
 *
 * `manifest` 与 `descriptor` 是「我给中台什么」，`validate` 与 `handle` 是
 * 「我干什么」——四者缺一不可：契约校验、编排连线、MCP 工具聚合都建立在它们之上。
 *
 * 与 Go 侧一样，用**接口**而不是基类：插件作者不必继承任何东西，也就不必关心
 * SDK 的内部结构。
 */
export interface Plugin {
  /**
   * 声明插件的身份、契约（消费/生产哪些消息类型）与暴露给 agent 的工具。
   *
   * ⚠️ 同一版本号的 manifest 不可变更：中台会拒绝「同号不同契约」的注册，
   * 改了东西请升版本号。
   */
  manifest(): ManifestInput

  /**
   * 返回本插件的 `FileDescriptorSet`，中台据此建立契约基线并做字段级兼容检查。
   *
   * 只用 `google.protobuf.Struct` 承载 JSON 的插件返回 {@link emptyDescriptor} 即可。
   */
  descriptor(): Uint8Array

  /**
   * 数据校验规则。
   *
   * 中台在把数据交给 `handle` 之前**一定**先调用它；返回 `valid: false` 时链路短路，
   * `handle` 不会被调用。规则与插件体同版本发布，因此规则不可能与实现漂移。
   *
   * 返回 {@link valid} / {@link invalid} 构造的响应即可（见 `envelope.ts`）。
   */
  validate(ctx: CallContext, env: Envelope): ValidateResponse | Promise<ValidateResponse>

  /**
   * 插件体：自主实现的数据输入输出。
   *
   * 输入载荷用 {@link payloadJSON} 取，输出用 {@link withPayloadJSON} 装。
   * 返回 `null` 会被中台当成「插件异常」——要表达「拒绝处理」请**抛错**。
   */
  handle(ctx: CallContext, env: Envelope): Envelope | null | Promise<Envelope | null>
}

/**
 * 由需要外置状态的插件实现。
 *
 * 用可选接口而不是往 {@link Plugin} 里加方法：老插件一行不用改，新插件想用才实现。
 * 骨架在**每次注册成功后**调用它（含被摘除后自愈的那次重新注册），所以实现方
 * 必须自己保证同步——handler 可能跑在别的回调里。
 */
export interface StateAware {
  /** 注册成功后由骨架注入，凭证随每次注册轮换。 */
  setState(state: StateClient): void
}

/** 运行时判定一个插件对象是不是 {@link StateAware}。 */
export function isStateAware(plugin: Plugin): plugin is Plugin & StateAware {
  return typeof (plugin as Partial<StateAware>).setState === 'function'
}

/**
 * 由需要插件间发现/互调能力的插件实现。
 *
 * 与 {@link StateAware} 同款的可选接口：老插件一行不用改，注入同样发生在**每次**
 * 注册成功后（凭证轮换），实现方自己负责并发安全——Node 是单线程，一次引用赋值
 * 就是原子的，但 handler 与注册循环仍可能交错，别在读取后再假设它没被换过。
 */
export interface GatewayAware {
  /** 注册成功后由骨架注入，凭证随每次注册轮换。 */
  setGateway(gateway: GatewayClient): void
}

/** 运行时判定一个插件对象是不是 {@link GatewayAware}。 */
export function isGatewayAware(plugin: Plugin): plugin is Plugin & GatewayAware {
  return typeof (plugin as Partial<GatewayAware>).setGateway === 'function'
}
