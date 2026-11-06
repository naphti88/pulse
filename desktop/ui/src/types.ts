// What the Rust side hands over (src-tauri/src/items.rs)

export type State = 'running' | 'blocked' | 'waiting' | 'review' | 'quiet'
export type Stage = 'running' | 'open' | 'follow' | 'parked' | 'done'
export type Kind = 'recurring' | 'oneoff' | 'chat'

export type Live = { turnStarted?: number; tool?: string; tools: number; errors: number }

export type Subagent = {
  id: string
  type: string
  description: string
  startedAt: number
  lastAt: number
  isRunning: boolean
  isBackground: boolean
  depth: number
}

// src-tauri/src/rules.rs: which rules made an item what it is
export type Why = { rule: string; says: string; stageRule: string; stageSays: string; passed: string[]; facts: string[] }

export type Item = {
  id: string
  title: string
  project: string
  kind: Kind
  state: State
  stage: Stage
  since: number
  startedAt: number
  reason: string
  parentId?: string
  live?: Live
  subagents: Subagent[]
  // what it changed that has not landed, in words
  work: string[]
  why: Why
}

export type Planned = { title: string; project: string; kind: Kind; at: number; count: number; last: number }
export type Missed = { title: string; project: string; at: number }
export type Counts = { parked: number; blocked: number; waiting: number; review: number; running: number; follow: number; commands: number }

// a topic set aside; gone: the app no longer keeps its session, a note only
export type ParkedView = { id: string; title: string; project: string; said: string; at: number; gone: boolean }

// src-tauri/src/checks.rs: a command a session runs now
export type Flag = 'hot' | 'stuck' | 'forgotten' | 'long'
export type Check = {
  key: string
  sessionId?: string
  title: string
  project: string
  what: string
  background: boolean
  flag?: Flag
  started: number
  lastOutput: number
  cpu: number
  memory: number
  ports: number[]
  known: boolean
}

export type Snapshot = {
  now: number
  tzOffsetMin: number
  items: Item[]
  planned: Planned[]
  missed: Missed[]
  counts: Counts
  checks: Check[]
  parked: ParkedView[]
  problems: string[]
}

// src-tauri/src/history.rs: a session in the Verlauf
export type Entry = {
  id: string
  title: string
  project: string
  kind: Kind
  at: number
  state: State
  stage: Stage
  said: string
  archived: boolean
  parentId?: string
}

// src-tauri/src/settings.rs
export type Rule = { atOnce: boolean; remind: boolean }
export type Limits = { forgottenMin: number; longMin: number; stuckMin: number; hotMin: number }
export type Settings = {
  blocked: Rule
  waiting: Rule
  review: Rule
  commands: Rule
  limits: Limits
  remindHours: number[]
  remindDaily: boolean
  summary: boolean
  summaryAt: number
  parkedWeekly: boolean
  appearance: Appearance
}
export type Appearance = 'system' | 'light' | 'dark'
