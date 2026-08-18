// 模块级单实例 API 客户端（Spec B4「统一 API 客户端」：全仓单实例，会话/
// PAT 通道与 401 落登录逻辑只此一份，页面/组件不各自 new）。

import { createApiClient } from './http'

/** 单实例客户端句柄（endpoints 见 `client.ts`）。 */
export const http = createApiClient()
