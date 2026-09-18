import type { ArchiveStatus } from '@/api/types'

/** 日志可用性独立于任务执行结果，两个展示面使用同一标签规则。 */
export function archiveStateLabelKey(archive: Pick<ArchiveStatus, 'state' | 'lost_reason'>): string {
  if (archive.state === 'pending') return 'logArchive.pending'
  if (archive.lost_reason === 'retention_expired') return 'logArchive.expired'
  return 'logArchive.lost'
}
