// The symbols of Pulse, 18×18 and filled; the same shapes as the menu bar's
// (design/mockups/glyphs.js, rendered there by make_tray.py). A cut-out part
// takes --knock, the ground behind the symbol.

import type { Kind, Stage, State } from './types'

const SVG: Record<string, string> = {
  blocked: `<path d="M6 1h6l5 5v6l-5 5H6l-5-5V6z" fill="currentColor"/><path d="M9 4.8v5" stroke="var(--knock)" stroke-width="2.1" stroke-linecap="round"/><circle cx="9" cy="12.9" r="1.2" fill="var(--knock)"/>`,
  waiting: `<path d="M3 2.5h12A2 2 0 0117 4.5v7.2a2 2 0 01-2 2H8.4L4.6 16.6v-2.9H3a2 2 0 01-2-2V4.5a2 2 0 012-2z" fill="currentColor"/><path d="M7.2 6.3a1.9 1.9 0 113 1.5c-.6.4-1.2.8-1.2 1.6" fill="none" stroke="var(--knock)" stroke-width="1.6" stroke-linecap="round"/><circle cx="9" cy="11.3" r=".95" fill="var(--knock)"/>`,
  review: `<path d="M1 9c2-3.8 4.8-5.6 8-5.6S15 5.2 17 9c-2 3.8-4.8 5.6-8 5.6S3 12.8 1 9z" fill="currentColor"/><circle cx="9" cy="9" r="3" fill="var(--knock)"/><circle cx="9" cy="9" r="1.5" fill="currentColor"/>`,
  running: `<circle cx="9" cy="9" r="6.6" fill="none" stroke="currentColor" stroke-opacity=".25" stroke-width="2.6"/><path d="M9 2.4a6.6 6.6 0 016.6 6.6" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round"/><circle cx="9" cy="9" r="2" fill="currentColor"/>`,
  commands: `<path d="M3.2 1.6h11.6M3.2 16.4h11.6" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/><path d="M4.8 2.4h8.4v2.4c0 1.7-1.3 3-3 4.2 1.7 1.2 3 2.5 3 4.2v2.4H4.8v-2.4c0-1.7 1.3-3 3-4.2-1.7-1.2-3-2.5-3-4.2z" fill="currentColor"/><path d="M6.5 4.1h5c-.1.5-.4 1-.8 1.4H7.3c-.4-.4-.7-.9-.8-1.4zM9 11c-1 .6-1.9 1.4-2.3 2.6v.5h4.6v-.5C10.9 12.4 10 11.6 9 11z" fill="var(--knock)"/>`,
  // what a session does while it calls nothing: a spark, in Claude's colour
  thinking: `<path d="M9 1.5v15M1.5 9h15M3.7 3.7l10.6 10.6M14.3 3.7L3.7 14.3" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"/>`,
  // a topic set aside
  parked: `<circle cx="9" cy="9" r="7.6" fill="currentColor"/><path d="M7 5.9v6.2M11 5.9v6.2" stroke="var(--knock)" stroke-width="2" stroke-linecap="round"/>`,
  // goes on again: the play to parked's pause
  unpark: `<path d="M5 2.8v12.4a.8.8 0 001.2.7l10-6.2a.8.8 0 000-1.4l-10-6.2A.8.8 0 005 2.8z" fill="currentColor"/>`,
  done: `<circle cx="9" cy="9" r="7.6" fill="currentColor"/><path d="M5.4 9.3l2.4 2.4 4.8-5" fill="none" stroke="var(--knock)" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/>`,
  follow: `<path d="M4 1.8v14.4" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/><path d="M4.6 2.6h10l-2.6 3.6 2.6 3.6h-10z" fill="currentColor"/>`,
  missed: `<circle cx="9" cy="9" r="6.8" fill="none" stroke="currentColor" stroke-width="1.8" stroke-dasharray="2.6 2.2"/><path d="M6.4 6.4l5.2 5.2M11.6 6.4l-5.2 5.2" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  planned: `<circle cx="9" cy="9" r="7" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M9 4.8V9l2.8 1.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  recurring: `<path d="M3.2 8A5.9 5.9 0 0113.6 5M14.8 10A5.9 5.9 0 014.4 13" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M14.4 2.2v3.4H11M3.6 15.8v-3.4H7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  oneoff: `<rect x="2.2" y="3.4" width="13.6" height="12.4" rx="2.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M2.4 7.4h13.2M6 1.8v3M12 1.8v3" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><circle cx="9" cy="11.4" r="1.6" fill="currentColor"/>`,
  // what an item is: a session is a window, you started it (prompt) or Claude
  // did (spark); a subagent is a small window hanging off its session
  session: `<rect x="1.8" y="2.6" width="14.4" height="12.8" rx="2.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M5.2 6.9l2.4 2.1-2.4 2.1" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/><path d="M9.6 11.2h3.2" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  claude: `<rect x="1.8" y="2.6" width="14.4" height="12.8" rx="2.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M9 4.9C9.3 7.4 9.8 8.1 12.4 9 9.8 9.9 9.3 10.6 9 13.1 8.7 10.6 8.2 9.9 5.6 9 8.2 8.1 8.7 7.4 9 4.9z" fill="currentColor"/>`,
  agent: `<circle cx="4.4" cy="3.9" r="2.3" fill="currentColor"/><path d="M4.4 6.4v4.4a2.4 2.4 0 002.4 2.4h2.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><rect x="9.6" y="9.9" width="6.6" height="6.6" rx="1.9" fill="none" stroke="currentColor" stroke-width="1.6"/>`,
  pin: `<path d="M11.6 1.8l4.6 4.6-1.4 1.4-1-.4-2.6 2.6.3 3.2-1.3 1.3-3.3-3.3-3.9 3.9-.9-.9 3.9-3.9-3.3-3.3 1.3-1.3 3.2.3 2.6-2.6-.4-1z" fill="currentColor"/>`,
  pinOff: `<path d="M11.6 1.8l4.6 4.6-1.4 1.4-1-.4-2.6 2.6.3 3.2-1.3 1.3-3.3-3.3-3.9 3.9-.9-.9 3.9-3.9-3.3-3.3 1.3-1.3 3.2.3 2.6-2.6-.4-1z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>`,
  external: `<path d="M10 2.5h5.5V8M15.2 2.8L8.5 9.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/><path d="M13 11v3a1.5 1.5 0 01-1.5 1.5h-7A1.5 1.5 0 013 14V7a1.5 1.5 0 011.5-1.5h3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  // back below the menu bar item: an arrow turning back
  recenter: `<path d="M4.4 6.6A5.6 5.6 0 1 1 3.6 11.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M3.6 2.8v3.9h3.9" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  // small and big again: arrows in to the middle, out to the corners
  shrink: `<path d="M2.8 2.8l4.4 4.4M7.2 3.4v3.8H3.4M15.2 15.2l-4.4-4.4M10.8 14.6v-3.8h3.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  grow: `<path d="M7.2 7.2L2.8 2.8M2.8 6.6V2.8h3.8M10.8 10.8l4.4 4.4M15.2 11.4v3.8h-3.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  close: `<path d="M5 5l8 8M13 5l-8 8" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>`,
  folder: `<path d="M1.8 4.6a1.6 1.6 0 011.6-1.6h3.4l1.8 1.8h6a1.6 1.6 0 011.6 1.6v7.4a1.6 1.6 0 01-1.6 1.6H3.4a1.6 1.6 0 01-1.6-1.6z" fill="currentColor"/>`,
  why: `<circle cx="9" cy="9" r="7.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M6.9 7.2a2.2 2.2 0 114.1 1.1c-.6.9-2 1.2-2 2.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><circle cx="9" cy="13.3" r="1" fill="currentColor"/>`,
  check: `<path d="M4 9.4l3.2 3.2L14 5.6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>`,
  gear: `<path d="M14.90 6.78L17.04 7.37V10.63L14.90 11.22L14.74 11.60L15.83 13.53L13.53 15.83L11.60 14.74L11.22 14.90L10.63 17.04H7.37L6.78 14.90L6.40 14.74L4.47 15.83L2.17 13.53L3.26 11.60L3.10 11.22L0.96 10.63V7.37L3.10 6.78L3.26 6.40L2.17 4.47L4.47 2.17L6.40 3.26L6.78 3.10L7.37 0.96H10.63L11.22 3.10L11.60 3.26L13.53 2.17L15.83 4.47L14.74 6.40z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/><circle cx="9" cy="9" r="2.6" fill="none" stroke="currentColor" stroke-width="1.5"/>`,
  eye: `<path d="M1.5 9c1.9-3.5 4.5-5.2 7.5-5.2s5.6 1.7 7.5 5.2c-1.9 3.5-4.5 5.2-7.5 5.2S3.4 12.5 1.5 9z" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/><circle cx="9" cy="9" r="2.4" fill="currentColor"/>`,
}

export type GlyphName = keyof typeof SVG

export function Glyph({ name, size = 16, className = '', title }: { name: GlyphName; size?: number; className?: string; title?: string }) {
  return (
    <span className={`glyph ${className}`} style={{ width: size, height: size }} title={title}>
      <svg viewBox="0 0 18 18" dangerouslySetInnerHTML={{ __html: SVG[name] ?? '' }} />
    </span>
  )
}

// the symbol of a state, in its colour; running turns, blocked pulses
export function StateMark({ state, size = 16, done = false }: { state: State; size?: number; done?: boolean }) {
  if (done || state === 'quiet') return <Glyph name="done" size={size} className={done ? 's-done' : 's-quiet'} />
  const motion = state === 'running' ? 'spin' : state === 'blocked' ? 'pulse' : ''
  return <Glyph name={state} size={size} className={`s-${state} ${motion}`} />
}

// what an item wants from you: its state, or where you put it
export type Mark = State | 'parked' | 'follow' | 'done' | 'commands'
export const markOf = (i: { state: State; stage: Stage }): Mark =>
  i.stage === 'parked' ? 'parked' : i.stage === 'follow' ? 'follow' : i.stage === 'done' && i.state !== 'quiet' ? 'done' : i.state

// what an item is: a routine, a task planned once, a session you started or
// one Claude started from another
export type Type = 'recurring' | 'oneoff' | 'session' | 'claude'
export const typeOf = (i: { kind: Kind; parentId?: string }): Type => (i.kind !== 'chat' ? i.kind : i.parentId ? 'claude' : 'session')

export const TYPE_LABEL: Record<Type, string> = {
  recurring: 'Routine',
  oneoff: 'Planned once',
  session: 'Session, started by you',
  claude: 'Session, started by Claude',
}

// a mark as a disc in the colour of what the item wants, the symbol of what
// it is at its foot; what is open is filled, what waits on nobody is pale
export function Disc({ mark, type, small = false, alert = false }: { mark: Mark; type?: Type; small?: boolean; alert?: boolean }) {
  const pale = mark === 'parked' || mark === 'follow' || mark === 'done' || mark === 'quiet'
  return (
    <span className={`disc ${small ? 'small' : ''} m-${mark} ${pale ? 'pale' : ''} ${alert ? 'alert' : ''}`}>
      <Glyph name={mark === 'quiet' ? 'done' : mark} size={small ? 13 : 18} className={mark === 'running' ? 'spin' : ''} />
      {type && !small && (
        <span className="badge" title={TYPE_LABEL[type]}>
          <Glyph name={type} size={12} />
        </span>
      )}
    </span>
  )
}

export const STATE_LABEL: Record<State, string> = {
  running: 'running',
  blocked: 'blocked',
  waiting: 'question',
  review: 'result to review',
  quiet: 'nothing to do',
}
