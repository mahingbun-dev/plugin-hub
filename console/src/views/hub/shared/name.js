/**
 * flow 名的字符集与校验——列表页新建、编辑器改名共用这一份。
 *
 * 与中台 `hub_flow::NAME_PATTERN` 保持一致：名字要能进 URL、能当 MCP 工具名的一部分。
 * 校验规则一旦在两个页面各自手写，迟早漂移成两套标准。
 */
export const NAME_PATTERN = /^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$/;

export const NAME_HINT = "流程名只能由字母、数字、下划线、中划线组成，且以字母或数字开头";

export function isValidFlowName(name) {
  return NAME_PATTERN.test(name);
}
