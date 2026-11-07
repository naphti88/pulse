import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useEffect, useState } from 'react'

import type { Entry, Settings, Snapshot } from './types'

// The Rust side works everything out and sends a new snapshot whenever the
// files change; the panel asks once when it starts.
export function useSnapshot() {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null)
  const [error, setError] = useState<string | null>(null)
  useEffect(() => {
    invoke<Snapshot>('snapshot').then(setSnapshot, e => setError(String(e)))
    const stop = listen<Snapshot>('snapshot', e => {
      setSnapshot(e.payload)
      setError(null)
    })
    return () => void stop.then(f => f())
  }, [])
  return { snapshot, error }
}

export const mark = (ids: string[], kind: 'seen' | 'done' | 'open' | 'park' | 'resume' | 'forget', shown?: number) => invoke('mark', { ids, kind, shown })
// a chat that did not open says why in the foot a while: the Rust side
// rejects with a short sentence, such as "Codex is not installed."
const failures = new Set<(why: string) => void>()
const failed = (e: unknown) => failures.forEach(f => f(String(e)))
export function useFailure() {
  const [why, setWhy] = useState<string | null>(null)
  useEffect(() => {
    let timer: number | undefined
    const show = (w: string) => {
      setWhy(w)
      clearTimeout(timer)
      timer = window.setTimeout(() => setWhy(null), 8000)
    }
    failures.add(show)
    return () => {
      failures.delete(show)
      clearTimeout(timer)
    }
  }, [])
  return why
}
export const openChat = (id: string) => invoke('open_chat', { id }).catch(failed)
export const saveCase = (id: string) => invoke<string>('save_case', { id })
export const setPinned = (pinned: boolean) => invoke<boolean>('set_pinned', { pinned })
export const closePanel = () => invoke('close_panel')
export const panelSize = (height: number, width?: number) => invoke('panel_size', { width, height })
export const setCompact = (compact: boolean) => invoke('set_compact', { compact })

// whether the panel stands at a place of its own, kept up to date
export function usePlaced() {
  const [placed, setPlaced] = useState(false)
  useEffect(() => {
    invoke<boolean>('is_placed').then(setPlaced, () => undefined)
    const stop = listen<boolean>('placed', e => setPlaced(e.payload))
    return () => void stop.then(f => f())
  }, [])
  return placed
}
export const resetPlace = () => invoke('reset_place')
export const markCommand = (key: string, known: boolean) => invoke('mark_command', { key, known })
export const getHistory = () => invoke<Entry[]>('history')
export const getSettings = () => invoke<{ settings: Settings; problem?: string }>('get_settings')
export const saveSettings = (settings: Settings) => invoke<Settings>('set_settings', { settings })
