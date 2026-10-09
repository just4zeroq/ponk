/** QueryClient 配置（§5.6 轮询节奏见各页面）。 */
import { QueryClient } from '@tanstack/vue-query'

export function createAppQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        // 运行时数据 5s 心跳（10 §5）→ 默认 5s 保持新鲜；各页按 §5.6 覆盖。
        staleTime: 5_000,
        gcTime: 60_000,
        retry: (failureCount, error) => {
          // 401/403/404/409/412/428/400 不重试；5xx 与网络错误最多 1 次。
          const status = (error as { status?: number })?.status
          if (status && status >= 400 && status < 500 && status !== 429) return false
          return failureCount < 1
        },
        refetchOnWindowFocus: false,
      },
      mutations: { retry: false },
    },
  })
}
