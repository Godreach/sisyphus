import { http } from './http-singleton'
import type { ArchiveStatus } from './types'

/** 状态与正文 API 分离；查询不刷新归档保留期。 */
export const logArchivesApi = {
  status: (path: string) => http.get<ArchiveStatus | null>(path),
  backlog: (agent: string, offset = 0) => http.get<ArchiveStatus[]>('log-archives', { query: { agent, offset } }),
  markLost: (archive: ArchiveStatus, reason: string) =>
    http.post<ArchiveStatus>(`log-archives/${archive.job_id}/${archive.attempt}/lost`, { json: { reason } }),
}
