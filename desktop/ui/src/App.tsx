import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { listen } from '@tauri-apps/api/event'

import { closePanel, panelSize, resetPlace, setCompact, setPinned, useFailure, usePlaced, useSnapshot } from './api'
import { agoShort, clock } from './format'
import { Glyph, StateMark } from './glyphs'
import { RunCard, useNow } from './parts'
import { HistoryTab } from './history'
import { SettingsView } from './settings'
import { OpenTab, TimelineTab } from './tabs'
import type { Counts as CountsT } from './types'
import appIcon from '../../assets/app-icon.svg?raw'
import appIconLight from '../../assets/app-icon-light.svg?raw'

// the app icon cut to its rounded square, in the foot: dark or light as the panel is
const cut = (svg: string) => svg.replace(/viewBox="[^"]*" width="\d+" height="\d+"/, 'viewBox="81 88 862 862"')
const LOGO = cut(appIcon)
const LOGO_LIGHT = cut(appIconLight)

type Tab = 'open' | 'timeline' | 'history'
const TABS: Tab[] = ['open', 'timeline', 'history']

function useStored<T extends string>(key: string, initial: T): [T, (v: T) => void] {
  const [value, setValue] = useState<T>(() => {
    try {
      return (localStorage.getItem(key) as T | null) ?? initial
    } catch {
      return initial
    }
  })
  return [
    value,
    v => {
      setValue(v)
      try {
        localStorage.setItem(key, v)
      } catch {
        // without storage it is kept for this run only
      }
    },
  ]
}

// the count of each open state that has one; small, a click on one makes the
// panel big again, and what runs counts when nothing else is open
function Counts({ c, small = false, onOpen }: { c?: CountsT; small?: boolean; onOpen?: () => void }) {
  if (!c) return null
  const open = c.blocked + c.waiting + c.review + c.commands
  const pill = (state: string, mark: ReactNode, text: string) => (
    <span className={`cnt s-${state}`} title={onOpen ? 'Make big' : undefined} onClick={onOpen}>
      {mark}
      {text}
    </span>
  )
  return (
    <>
      {open + c.running === 0 && pill('calm', <Glyph name="done" size={15} />, 'All quiet')}
      {small && open === 0 && c.running > 0 && pill('running', <Glyph name="running" size={15} className="spin" />, `${c.running} running`)}
      {c.blocked > 0 && pill('blocked', <StateMark state="blocked" size={15} />, `${c.blocked} blocked`)}
      {c.waiting > 0 && pill('waiting', <StateMark state="waiting" size={15} />, `${c.waiting} ${c.waiting === 1 ? 'question' : 'questions'}`)}
      {c.commands > 0 && pill('commands', <Glyph name="commands" size={15} />, `${c.commands} ${c.commands === 1 ? 'command' : 'commands'}`)}
      {c.review > 0 && pill('review', <StateMark state="review" size={15} />, `${c.review} to review`)}
    </>
  )
}

export function App() {
  const { snapshot: snap, error } = useSnapshot()
  const now = useNow()
  const [stored, setTab] = useStored<Tab>('tab', 'open')
  // a tab kept from before that is gone opens Open
  const tab = TABS.includes(stored) ? stored : 'open'
  const [pinned, setPinnedState] = useState(false)
  const [isSettings, setIsSettings] = useState(false)
  const placed = usePlaced()
  // why a chat just did not open, in the foot for a while
  const failure = useFailure()
  // small: only the counts, as a bar that stays open
  const [size, setSize] = useStored<'big' | 'small'>('size', 'big')
  const small = size === 'small'
  useEffect(() => void setCompact(small), [small])
  // a click on the menu bar item or on a notification opens it big
  useEffect(() => {
    const stop = listen<'big'>('size', () => setSize('big'))
    return () => void stop.then(f => f())
  }, [])
  const bar = useRef<HTMLDivElement>(null)
  const head = useRef<HTMLDivElement>(null)
  const body = useRef<HTMLDivElement>(null)
  const foot = useRef<HTMLDivElement>(null)

  // the window is as tall as its content, up to a limit; beyond it the list
  // scrolls. The content is measured, not the list, which fills the window
  // and so would never let it shrink again
  useLayoutEffect(() => {
    const measure = () => {
      if (small) {
        if (bar.current) void panelSize(bar.current.offsetHeight, bar.current.offsetWidth)
        return
      }
      const content = body.current?.firstElementChild as HTMLElement | null | undefined
      const h = (head.current?.offsetHeight ?? 0) + (content?.offsetHeight ?? 0) + (foot.current?.offsetHeight ?? 0)
      void panelSize(h)
    }
    const observer = new ResizeObserver(measure)
    for (const el of [bar.current, head.current, body.current?.firstElementChild, foot.current]) if (el) observer.observe(el)
    measure()
    return () => observer.disconnect()
  }, [tab, snap, isSettings, small])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') void closePanel()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const c = snap?.counts
  const open = c ? c.blocked + c.waiting + c.review + c.commands : 0
  const togglePin = () => void setPinned(!pinned).then(setPinnedState)

  if (small) {
    return (
      <div className="panel small" data-tauri-drag-region>
        <div ref={bar} className="bar" data-tauri-drag-region>
          <Counts c={c} small onOpen={() => setSize('big')} />
          {!c && <span className="cnt s-calm">…</span>}
          <button className="tb" title="Make big" onClick={() => setSize('big')}>
            <Glyph name="grow" size={14} />
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="panel">
      {/* the head moves the window: its free space, not its buttons */}
      <div ref={head} className="hdr" data-tauri-drag-region>
        <div className={`counts ${c && open === 0 && c.running > 0 ? 'none' : ''}`} data-tauri-drag-region>
          <Counts c={c} />
        </div>
        <div className="tabs" data-tauri-drag-region>
          {(
            [
              ['open', 'Open'],
              ['timeline', 'Timeline'],
              ['history', 'History'],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              className={tab === id && !isSettings ? 'on' : ''}
              onClick={() => {
                setTab(id)
                setIsSettings(false)
              }}
            >
              {label}
              {id === 'open' && open > 0 && <small>{open}</small>}
            </button>
          ))}
          <span className="tools" data-tauri-drag-region>
            <button className={`tb ${isSettings ? 'on' : ''}`} title="Settings" onClick={() => setIsSettings(v => !v)}>
              <Glyph name="gear" size={15} />
            </button>
            {placed && (
              <button className="tb" title="Back to its place by the icon" onClick={() => void resetPlace()}>
                <Glyph name="recenter" size={15} />
              </button>
            )}
            <button className="tb" title="Make small: only the counts" onClick={() => setSize('small')}>
              <Glyph name="shrink" size={15} />
            </button>
            <button className={`tb ${pinned ? 'on' : ''}`} title={pinned ? 'Unpin: closes on a click outside' : 'Pin: stays open'} onClick={togglePin}>
              <Glyph name={pinned ? 'pin' : 'pinOff'} size={15} />
            </button>
            {pinned && (
              <button className="tb" title="Close" onClick={() => void closePanel()}>
                <Glyph name="close" size={15} />
              </button>
            )}
          </span>
        </div>
      </div>
      <div ref={body} className="body">
        <div>
          {!snap && !error && <div className="empty">loading …</div>}
          {error && <div className="empty error">{error}</div>}
          {isSettings && <SettingsView onDone={() => setIsSettings(false)} />}
          {snap && !isSettings && tab === 'open' && <RunCard items={snap.items.filter(i => i.stage === 'running')} now={now} />}
          {snap && !isSettings && tab === 'open' && <OpenTab snap={snap} now={now} />}
          {snap && !isSettings && tab === 'timeline' && <TimelineTab snap={snap} now={now} />}
          {snap && !isSettings && tab === 'history' && <HistoryTab version={snap.now} now={now} />}
        </div>
      </div>
      <div ref={foot} className="foot">
        <span className="brand">
          <span className="logo dark" title="Pulse" dangerouslySetInnerHTML={{ __html: LOGO }} />
          <span className="logo light" title="Pulse" dangerouslySetInnerHTML={{ __html: LOGO_LIGHT }} />
          {snap && <span title={`Updated at ${clock(snap.now)}`}>Updated {agoShort(snap.now, now)}</span>}
        </span>
        {failure ? (
          <span className="problem" title={failure}>
            ⚠ {failure}
          </span>
        ) : (
          snap && snap.problems.length > 0 && <span className="problem" title={snap.problems.join('\n')}>⚠ {snap.problems[0]}</span>
        )}
        {snap && c && c.follow > 0 && tab !== 'open' && <span>{c.follow} later</span>}
      </div>
    </div>
  )
}
